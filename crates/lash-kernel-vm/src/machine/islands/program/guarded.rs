//! Compile kind comparisons from their source operands.

use super::*;

impl Build<'_> {
    pub(super) fn kind(&mut self, expr: &Expr, lib: LibId, args: &[Expr]) -> Option<bool> {
        let Some((inner, literal)) = crate::compile::kind_test::test(self.exe, lib, args) else {
            return Some(false);
        };
        let Kind::Operator {
            args: inner_args, ..
        } = &inner.kind
        else {
            return Some(false);
        };
        if [inner, expr]
            .iter()
            .any(|expr| matches!(&expr.kind, Kind::Operator { closes, .. } if closes.len() > 1))
        {
            return Some(false);
        }
        let kind = operator_site(inner, self.exe)?;
        let same = operator_site(expr, self.exe)?;
        if kind.counted.is_some() || same.counted.is_some() {
            return Some(false);
        }
        self.site(kind.depth, kind.body_when_observed);
        self.site(same.depth, same.body_when_observed);
        if let Some(reads) = self.reads(std::slice::from_ref(&inner_args[0])) {
            let [read] = reads.into_vec().try_into().ok()?;
            self.open = None;
            self.emit(Op::Kind {
                site: Box::new(KindTest {
                    kind,
                    same,
                    literal,
                    memo: Default::default(),
                }),
                input: KindInput::Source(read),
            });
            self.result(expr.reg, &[inner_args[0].reg]);
            return Some(true);
        }
        self.expr(&inner_args[0])?;
        self.open = None;
        self.emit(Op::Kind {
            site: Box::new(KindTest {
                kind,
                same,
                literal,
                memo: Default::default(),
            }),
            input: KindInput::Register(inner_args[0].reg),
        });
        self.result(expr.reg, &[inner_args[0].reg]);
        Some(true)
    }
}
