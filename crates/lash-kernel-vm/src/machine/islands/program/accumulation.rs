//! A binary local update retains its operand order and source write.

use super::*;

impl Build<'_> {
    pub(super) fn fuse_accumulation(&mut self) {
        let end = self.ops.len();
        if end < 2 {
            return;
        }
        let (Op::DirectOperator { site, reads, .. }, Op::Write { site: write }) =
            (&self.ops[end - 2].op, &self.ops[end - 1].op)
        else {
            return;
        };
        if write.bind
            || write.shared
            || write.close.is_some()
            || write.reg != site.reg
            || site.checked
            || site.counted.is_some()
            || site.root.is_some()
            || site.act.is_some()
            || reads.len() != 2
            || !matches!(reads.first(), Some(Read::Local { at, shared: false, .. }) if *at == write.at)
        {
            return;
        }
        let Some(mut written) = self.ops.pop() else {
            unreachable!("the trailing write was matched")
        };
        let Some(mut operator) = self.ops.pop() else {
            unreachable!("the preceding operator was matched")
        };
        let Op::Write { site: write } = written.op else {
            unreachable!()
        };
        if write.accumulates {
            let Some(Instruction {
                op: Op::Accumulate(at),
                ..
            }) = self.ops.last()
            else {
                unreachable!()
            };
            assert_eq!(*at, write.at);
            let Some(accumulation) = self.ops.pop() else {
                unreachable!("the accumulation marker was matched")
            };
            operator.begin = accumulation.begin;
            operator.live = accumulation.live;
        }
        let Op::DirectOperator {
            site,
            reads,
            covered,
        } = operator.op
        else {
            unreachable!()
        };
        let function = self.exe.lib(site.lib);
        let append = function.text_append(self.exe).map(|_| {
            Box::new(Append {
                charge: function.charge.text_charge(),
            })
        });
        if append.is_some() {
            let extra = site
                .entry
                .deepest(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id));
            self.site(Depth::Template(extra), site.body_when_observed);
        }
        written.op = Op::Accumulation {
            site,
            append,
            reads,
            covered,
            write,
        };
        written.begin = operator.begin;
        written.live = operator.live;
        self.ops.push(written);
    }
}
