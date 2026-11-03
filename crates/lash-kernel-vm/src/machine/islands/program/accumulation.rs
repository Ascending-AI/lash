//! A binary local update retains its operand order and source write.

use super::*;

impl Build<'_> {
    pub(super) fn text_write(
        &self,
        native_pc: u32,
        write_pc: u32,
        physical: usize,
        close: Option<Close>,
    ) -> Option<std::sync::Arc<crate::compile::room::publication::text::TextWrite>> {
        let placement = self.placement.as_ref()?.clone();
        let native = match &self.ops.last()?.op {
            Op::Operator { site, .. } if site.pc == native_pc => site.charges.clone()?,
            Op::Native { site, .. } if site.pc == native_pc => site.charges.clone()?,
            _ => return None,
        };
        let statement = self.statement?;
        let write = self.write_charges(statement, write_pc, close)?;
        crate::compile::room::publication::text::TextWrite::compile(
            self.exe, placement, statement, native_pc, write_pc, physical, native, write,
        )
        .ok()
    }

    pub(super) fn fuse_accumulation(&mut self) {
        if !self.fused {
            return;
        }
        let end = self.ops.len();
        if end < 2 {
            return;
        }
        if let (Op::Native { site, .. }, Op::Write { site: write }) =
            (&self.ops[end - 2].op, &self.ops[end - 1].op)
            && write.text.is_some()
            && !write.shared
            && !write.bind
            && write.reg == site.reg
        {
            let Some(mut written) = self.ops.pop() else {
                unreachable!()
            };
            let Some(native) = self.ops.pop() else {
                unreachable!()
            };
            let Op::Write { site: write } = written.op else {
                unreachable!()
            };
            let Op::Native { site, inputs } = native.op else {
                unreachable!()
            };
            written.op = Op::TextWrite {
                site,
                text_inputs: inputs,
                write,
            };
            written.begin = native.begin;
            written.live = native.live;
            self.ops.push(written);
            return;
        }
        let (Op::Operator { site, inputs, .. }, Op::Write { site: write }) =
            (&self.ops[end - 2].op, &self.ops[end - 1].op)
        else {
            return;
        };
        let direct = matches!(inputs, OperatorInputs::Source(reads) if reads.len() == 2 && matches!(reads.first(), Some(Read::Local { at, shared: false, .. }) if *at == write.at));
        if write.bind
            || write.shared
            || (write.close.is_some() && write.text.is_none())
            || write.reg != site.reg
            || site.checked
            || site.counted.is_some()
            || site.root.is_some()
            || site.act.is_some()
            || (!direct && write.text.is_none())
        {
            return;
        }
        if matches!(inputs, OperatorInputs::Registers(args) if args.len() == 2 && end >= 3 && matches!(&self.ops[end - 3].op, Op::Native { site: join, .. } if matches!(join.direct(self.exe), Some(Direct::TextJoin)) && args[1] == join.reg))
        {
            return;
        }
        let Some(mut written) = self.ops.pop() else {
            unreachable!("the trailing write was matched")
        };
        let Some(operator) = self.ops.pop() else {
            unreachable!("the preceding operator was matched")
        };
        let Op::Write { site: write } = written.op else {
            unreachable!()
        };
        let Op::Operator {
            site,
            inputs,
            covered,
            ..
        } = operator.op
        else {
            unreachable!()
        };
        let function = self.exe.lib(site.lib);
        if function.text_append(self.exe).is_some() {
            self.site(
                Depth::Template(function.prepared_depth(self.exe)),
                site.body_when_observed,
            );
        }
        written.op = Op::Operator {
            site,
            inputs,
            covered,
            to: Destination::Write(write),
        };
        written.begin = operator.begin;
        written.live = operator.live;
        self.ops.push(written);
    }
}
