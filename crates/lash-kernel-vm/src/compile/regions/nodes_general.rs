//! Prepared scalar calls under a loop region's fixed charge policy.

use super::*;

/// Region nodes run only under their executable's charged/pace policy.
/// Helper bodies use register instructions, so this policy is fixed once.
pub(super) fn general_unary<const CHARGING: bool>(
    answer: Answer,
    a: Leaf,
    site: calls::Site,
) -> Node {
    Box::new(move |slots, cx| {
        let x = a.eval(slots, cx)?;
        let answered = if scalar(&x) {
            answer.answer::<CHARGING>(&[x])
        } else {
            None
        };
        match answered {
            Some((value, units)) => {
                if CHARGING {
                    cx.meter.measured(units);
                }
                Some(value)
            }
            None => site.call(&[x], cx),
        }
    })
}

pub(super) fn general_pair<const CHARGING: bool>(
    answer: Answer,
    pair: lash_kernel_doc::scalar::Pair,
    a: Leaf,
    b: Leaf,
    site: calls::Site,
) -> Node {
    Box::new(move |slots, cx| {
        let x = a.eval(slots, cx)?;
        let y = b.eval(slots, cx)?;
        let Some(value) = typed(&pair, x, y) else {
            return site.call(&[x, y], cx);
        };
        if !CHARGING {
            return Some(value);
        }
        if scalar(&value) {
            cx.meter
                .measured(answer.table.units(pair_sizes(x, y, value))?);
            Some(value)
        } else {
            site.charged(&[x, y], value.into(), cx)
        }
    })
}
