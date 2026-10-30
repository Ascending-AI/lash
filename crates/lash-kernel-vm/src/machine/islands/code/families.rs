//! Family projections are checked against the source AST, which was itself
//! independently verified from KIR. The grouping passes read physical IR.

use super::*;
use crate::compile::expr::{Expr, Kind as Expression};
use crate::compile::islands::{Block as SourceBlock, Kind as StatementKind, Refused, Statement};

fn unpin(mut expr: &Expr) -> &Expr {
    while let Expression::Pin(value) = &expr.kind {
        expr = value;
    }
    expr
}

fn write(block: &SourceBlock, source_pc: u32) -> Option<&Statement> {
    for statement in &block.statements {
        let found = match &statement.kind {
            StatementKind::Write { pc, .. } if *pc == source_pc => Some(statement),
            StatementKind::Try {
                body,
                catch,
                finally,
            } => write(body, source_pc)
                .or_else(|| catch.as_ref().and_then(|b| write(b, source_pc)))
                .or_else(|| finally.as_ref().and_then(|b| write(b, source_pc))),
            StatementKind::While {
                body, otherwise, ..
            }
            | StatementKind::For {
                body, otherwise, ..
            } => write(body, source_pc)
                .or_else(|| otherwise.as_ref().and_then(|b| write(b, source_pc))),
            StatementKind::If { then_, else_, .. } => {
                write(then_, source_pc).or_else(|| write(else_, source_pc))
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn written<'a>(site: &WriteSite, plan: &'a IslandPlan) -> Option<&'a Expr> {
    let source = write(&plan.body, site.pc)?;
    let StatementKind::Write {
        at,
        reg,
        bind,
        close,
        value,
        ..
    } = &source.kind
    else {
        return None;
    };
    (*at as usize == site.at
        && *reg == site.reg
        && *bind == site.bind
        && *close == site.close
        && source.leaves == site.leaves
        && plan.shared[site.at] == site.shared)
        .then_some(unpin(value))
}

fn literal(value: Value, source: &Expr) -> bool {
    match (value, &unpin(source).kind) {
        (Value::Int(a), Expression::Const(Value::Int(b))) => a == *b,
        (Value::Float(a), Expression::Const(Value::Float(b))) => a == *b,
        (Value::Bool(a), Expression::Const(Value::Bool(b))) => a == *b,
        (Value::Text(a), Expression::Const(Value::Text(b))) => a == *b,
        (Value::Null, Expression::Const(Value::Null))
        | (Value::Absent, Expression::Const(Value::Absent)) => true,
        _ => false,
    }
}

fn operand(operand: &Operand, source: &Expr, plan: &IslandPlan) -> bool {
    if operand.reg() != source.reg {
        return false;
    }
    match operand {
        Operand::Register(_) => true,
        Operand::Literal { value, .. } => literal(*value, source),
        Operand::Local { at, .. } | Operand::Cell { at, .. } => {
            matches!(unpin(source).kind, Expression::Local { local, scoped: false, .. } if local.at as usize == *at)
                && plan.shared[*at] == matches!(operand, Operand::Cell { .. })
        }
    }
}

fn operator<'a>(site: &OperatorSite, source: &'a Expr) -> Option<&'a [Expr]> {
    let source = unpin(source);
    let Expression::Operator {
        lib,
        args,
        closes,
        invoke,
    } = &source.kind
    else {
        return None;
    };
    (site.lib == *lib
        && site.pc == source.pc
        && site.reg == source.reg
        && closes.len() <= 1
        && site.close == closes.first().copied()
        && site.checked == invoke.is_some_and(|call| call.checked)
        && site.counted == invoke.and_then(|call| call.counted()))
    .then_some(args)
}

fn scalar_kernel(site: &OperatorSite, exe: &Executable) -> bool {
    let lib = exe.lib(site.lib);
    crate::compile::numeric_pair(lib, site.checked).is_some()
        && lib
            .specialization
            .prepared
            .as_ref()
            .and_then(|(_, fast)| fast.scalar())
            .is_some()
}

fn read_charge(site: &Update, exe: &Executable) -> bool {
    let lib = exe.lib(site.read.lib);
    let Some(path) = lib
        .specialization
        .prepared
        .as_ref()
        .and_then(|(entry, _)| entry.element())
    else {
        return false;
    };
    if path.room != 2 {
        return false;
    }
    use lash_kernel_doc::plan::Element;
    match (path.operation, site.charge) {
        (Element::ReadMap, MemberAccess::Map(actual)) => {
            match (actual, lib.charge.short_text_charge(1)) {
                (None, None) => true,
                (Some(actual), Some(canonical)) => (0..=lash_kernel_doc::arena::ShortText::MAX
                    as u64)
                    .all(|length| actual.units(length) == canonical.units(length)),
                _ => false,
            }
        }
        (Element::ReadInt, MemberAccess::Int { zero, nonzero }) => {
            lib.charge.known_charge(&[None, Some(Value::Int(0))], None) == Some(zero)
                && lib.charge.known_charge(&[None, Some(Value::Int(1))], None) == Some(nonzero)
        }
        (Element::Read, MemberAccess::Float(units)) => {
            lib.charge.known_charge(
                &[None, Some(Value::Float(lash_kernel_doc::Float::new(0.0)))],
                None,
            ) == Some(units)
        }
        _ => false,
    }
}

impl Update {
    fn source(&self, plan: &IslandPlan, exe: &Executable) -> Option<()> {
        if !scalar_kernel(&self.scalar, exe) || !read_charge(self, exe) {
            return None;
        }
        let value = written(&self.write, plan)?;
        let statement = write(&plan.body, self.write.pc)?;
        let args = operator(&self.scalar, value)?;
        let [a, b] = args else {
            return None;
        };
        if !operand(&self.left, a, plan)
            || !operand(&self.right, b, plan)
            || self.consumed.as_ref() != [a.reg, b.reg]
        {
            return None;
        }
        let candidate = [unpin(a), unpin(b)]
            .into_iter()
            .find(|expr| expr.reg == self.read.reg)?;
        let read = if matches!(candidate.kind, Expression::Act) {
            statement.root.as_ref()?
        } else {
            candidate
        };
        if read.pc != self.read.pc {
            return None;
        }
        let (args, lib, close) = match &read.kind {
            Expression::Call {
                invoke,
                args,
                closes,
                ..
            } => {
                let lib = invoke
                    .act
                    .and_then(|act| plan.stream(exe).nested[act.ix()].kind.operator_lib())?;
                (args, lib, closes.first().copied())
            }
            Expression::Operator {
                lib, args, closes, ..
            } => (args, *lib, closes.first().copied()),
            _ => return None,
        };
        if self.read.lib != lib
            || self.read.close != close
            || args.len() != 2
            || self.args.as_ref() != args.iter().map(|expr| expr.reg).collect::<Vec<_>>()
        {
            return None;
        }
        if let Some(reads) = &self.reads {
            for (read, source) in reads.iter().zip(args.iter()) {
                if !operand(&Operand::from(*read), source, plan) {
                    return None;
                }
            }
        }
        Some(())
    }
}

impl Format {
    fn source(&self, plan: &IslandPlan, exe: &Executable) -> Option<()> {
        let source = written(&self.write, plan)?;
        let [left, joined] = operator(&self.append, source)? else {
            return None;
        };
        if self.append_args != [left.reg, joined.reg]
            || self.appending.as_ref() != self.append_args
            || self.writing.as_ref() != [source.reg]
        {
            return None;
        }
        let joined = unpin(joined);
        let Expression::Native { lib, args, closes } = &joined.kind else {
            return None;
        };
        let [parts, separator] = &**args else {
            return None;
        };
        if self.join.lib != *lib
            || self.join.pc != joined.pc
            || self.join.reg != joined.reg
            || self.join.close != closes.first().copied()
            || closes.len() > 1
            || !exe.lib(*lib).text_join()
            || self.join_args != [parts.reg, separator.reg]
            || self.separator.0 != separator.reg
            || !literal(self.separator.1, separator)
            || self.joined.as_ref() != [left.reg, parts.reg, separator.reg]
        {
            return None;
        }
        let Expression::Make {
            make,
            items,
            ahead,
            miss,
        } = &unpin(parts).kind
        else {
            return None;
        };
        let expected: Vec<_> = items.iter().map(|item| item.reg).collect();
        let replay_guard = *miss < parts.pc && items.first().is_some_and(|item| item.pc < *miss);
        (*make == crate::compile::expr::Make::List
            && self.parts.make == *make
            && self.parts.reg == parts.reg
            && self.parts.items.as_ref() == expected
            && self.parts.members == items.len()
            && self.parts.ahead == *ahead
            && self.parts.miss == *miss
            && self.parts.replay_guard == replay_guard)
            .then_some(())
    }
}

fn source_test(block: &SourceBlock, id: StmtId) -> Option<&Expr> {
    for statement in &block.statements {
        let found = match &statement.kind {
            StatementKind::If { cond, .. } | StatementKind::While { cond, .. }
                if statement.id == id =>
            {
                Some(cond)
            }
            StatementKind::Try {
                body,
                catch,
                finally,
            } => source_test(body, id)
                .or_else(|| catch.as_ref().and_then(|block| source_test(block, id)))
                .or_else(|| finally.as_ref().and_then(|block| source_test(block, id))),
            StatementKind::While {
                body, otherwise, ..
            }
            | StatementKind::For {
                body, otherwise, ..
            } => source_test(body, id)
                .or_else(|| otherwise.as_ref().and_then(|block| source_test(block, id))),
            StatementKind::If { then_, else_, .. } => {
                source_test(then_, id).or_else(|| source_test(else_, id))
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn verified_unit(site: &sites::Unit, plan: &IslandPlan, exe: &Executable) -> Option<()> {
    let Some(selected) = &site.site.source else {
        return Some(());
    };
    let test = source_test(&plan.body, selected.statement)?;
    let Expression::Operator { lib, args, .. } = &test.kind else {
        return None;
    };
    let [value, tuple] = &args[..] else {
        return None;
    };
    let stream = plan.stream(exe);
    let at = |pc: u32| stream.source_sites.get(pc as usize)?.as_deref();
    let binding = match &value.kind {
        Expression::Local { local, .. } => Some(local.at),
        _ => None,
    };
    (crate::compile::islands::unit_test(Some(exe.lib(*lib)), args)
        && *lib == site.site.lib
        && test.reg == site.site.reg
        && value.reg == site.src
        && selected.binding == binding
        && selected.test == exe.stmt(selected.statement).site
        && at(test.pc)? == &selected.comparison
        && at(value.pc)? == &selected.value
        && at(tuple.pc)? == &selected.tuple)
        .then_some(())
}

impl Code {
    pub(in crate::machine::islands) fn verify_families(
        &self,
        plan: &IslandPlan,
        exe: &Executable,
    ) -> Result<(), Refused> {
        for site in &self.sites.unit {
            verified_unit(site, plan, exe).ok_or(Refused::Calls)?;
        }
        for site in &self.sites.update {
            site.value.source(plan, exe).ok_or(Refused::Calls)?;
        }
        for site in &self.sites.format {
            site.value.source(plan, exe).ok_or(Refused::Calls)?;
        }
        Ok(())
    }
}
